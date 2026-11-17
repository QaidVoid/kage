//! Branch a new session from an existing one at a specific entry.
//!
//! [`fork`] copies the ancestry of the target entry into the
//! destination, in conversation order, as [`crate::tree::SessionTree`]
//! defines it. A source with branch markers contributes only the
//! entries the conversation still reaches, and the copy itself is
//! always linear. The destination header carries a fresh
//! [`SessionId`], a freshly-minted creation timestamp, and
//! `parent_session` / `parent_entry` fields linking back to the
//! source. All other header fields (cwd, model, system prompt) are
//! inherited.
//!
//! Forking does not mutate the source. After a fork, both files exist
//! independently and may diverge. A [`fork`] also copies the agents the
//! source had started by the fork point, with their own agents, into the
//! fork's `agents` directory under fresh session ids, and names them by
//! those ids throughout the copies. The fork then owns its agents, and
//! deleting the source leaves them whole. A fork made before this, and a
//! fork of one, still find theirs with the source through
//! [`crate::layout::agent_file`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::blobs::BlobStore;
use crate::entry::{EntryId, FORMAT_VERSION, Header, SessionEntry, SessionId};
use crate::error::SessionError;
use crate::reader::SessionReader;
use crate::tree::SessionTree;
use crate::writer::SessionWriter;

/// Fork `src` into a new session file at `dst`, copying entries up to and
/// including the entry whose id equals `at`. The destination's session id
/// is `new_session`, which the caller is expected to have used when
/// constructing `dst` (so the file name and the in-file id agree).
/// The destination's header is derived from the source's: a fresh id
/// and timestamp, inherited cwd, model and system prompt, and
/// `parent_session` / `parent_entry` linking back to the source.
///
/// Returns whether the source's tail was torn: the copy then ends at
/// the last whole entry before it.
///
/// # Errors
///
/// Errors if `src` cannot be opened, if its first entry is not a header,
/// if `dst` already exists, if an external backend is involved, or if no
/// entry in `src` has id `at`.
pub fn fork(
    src: &Path,
    dst: &Path,
    new_session: SessionId,
    at: EntryId,
) -> Result<bool, SessionError> {
    let mut reader = SessionReader::iter(src)?;
    let Some(first) = reader.next() else {
        // A torn first line is a crash mid-header, not an empty
        // session; name it the way every other reader does.
        return Err(if reader.torn_trailing() {
            SessionError::MissingHeader {
                path: src.to_path_buf(),
            }
        } else {
            SessionError::Empty {
                path: src.to_path_buf(),
            }
        });
    };
    let SessionEntry::Header(parent_header) = first? else {
        return Err(SessionError::MissingHeader {
            path: src.to_path_buf(),
        });
    };

    if parent_header.backend.is_some() {
        return Err(SessionError::ExternalFork {
            path: src.to_path_buf(),
        });
    }
    let new_header = Header {
        version: FORMAT_VERSION,
        session: new_session,
        id: EntryId::new(),
        ts: Utc::now(),
        cwd: parent_header.cwd.clone(),
        model: parent_header.model.clone(),
        system_prompt: parent_header.system_prompt.clone(),
        parent_session: Some(parent_header.session),
        backend: None,
        parent_entry: Some(at),
    };
    fork_tree(src, dst, new_header, at, true)
}

/// Fork `src` into a new session file at `dst` under the header
/// the caller supplies, copying the entries of `at`'s ancestry, in
/// conversation order. On a source with branch markers the copy takes
/// the path the conversation still reaches and is always linear. The
/// header's `session` should be the id the caller used for `dst` (so
/// the file name and the in-file id agree). Use this instead of
/// [`fork`] when the new session needs its own model or system
/// prompt, as forked agent children do. A supported older header version
/// is upgraded to the current format version in the destination. Labels
/// anchored to the source header are retargeted to the destination header.
///
/// Returns whether the source's tail was torn: the copy then ends at
/// the last whole entry of the source.
///
/// # Errors
///
/// Errors if `src` cannot be read, if its first entry is not a header,
/// if `dst` already exists, if an external backend is involved, or if no
/// entry in `src` has id `at`. An unsupported destination header version
/// is rejected before creating the destination, as is a header ID that
/// collides with a copied entry.
pub fn fork_as(
    src: &Path,
    dst: &Path,
    new_header: Header,
    at: EntryId,
) -> Result<bool, SessionError> {
    fork_tree(src, dst, new_header, at, false)
}

/// [`fork_as`], copying the source's agents too when `with_agents`.
fn fork_tree(
    src: &Path,
    dst: &Path,
    new_header: Header,
    at: EntryId,
    with_agents: bool,
) -> Result<bool, SessionError> {
    let tree = SessionTree::read(src)?;
    if tree.header().backend.is_some() {
        return Err(SessionError::ExternalFork {
            path: src.to_path_buf(),
        });
    }
    if new_header.backend.is_some() {
        return Err(SessionError::ExternalFork {
            path: dst.to_path_buf(),
        });
    }
    ensure_supported_version(dst, new_header.version)?;
    if !tree.contains(at) {
        return Err(SessionError::EntryNotFound {
            path: src.to_path_buf(),
            at,
        });
    }
    let ancestry = tree.ancestry(at);
    if ancestry.iter().any(|entry| entry.id() == new_header.id) {
        return Err(SessionError::Io {
            path: dst.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "destination header ID collides with a copied entry",
            ),
        });
    }
    let truncated = tree.truncated();
    let source_root = tree.header().id;
    let destination_root = new_header.id;
    let source_session = tree.header().session;
    let new_session = new_header.session;
    let encode = |source| SessionError::Encode {
        path: dst.to_path_buf(),
        source,
    };
    let entries: Vec<SessionEntry> = ancestry
        .into_iter()
        .map(|entry| {
            remap_root(entry, source_root, destination_root).unwrap_or_else(|| entry.clone())
        })
        .collect();
    let (agents, until) = match entries.last() {
        Some(last) if with_agents && crate::layout::agents_dir(src).is_dir() => {
            let mut named = HashSet::new();
            for entry in &entries {
                let line = serde_json::to_string(entry).map_err(encode)?;
                named.extend(ids_in(&line).map(str::to_owned));
            }
            (
                agents_started_by(src, source_session, &named, last.ts())?,
                last.ts(),
            )
        }
        _ => (Vec::new(), Utc::now()),
    };
    let mut renames: HashMap<String, String> = agents
        .iter()
        .map(|(id, _)| (id.clone(), SessionId::new().to_string()))
        .collect();
    let mut writer = SessionWriter::create(dst.to_path_buf(), new_header)?;
    for entry in &entries {
        if renames.is_empty() {
            writer.append(entry)?;
            continue;
        }
        let json = serde_json::to_string(entry).map_err(encode)?;
        let copy = serde_json::from_str(&rename_ids(&json, &renames)).map_err(encode)?;
        writer.append(&copy)?;
    }
    drop(writer);
    if !agents.is_empty() {
        // An agent's parent is the session it was copied with.
        renames.insert(source_session.to_string(), new_session.to_string());
        if let Err(err) = copy_agents(&agents, &crate::layout::agents_dir(dst), &renames, until) {
            let _ = std::fs::remove_file(dst);
            if let (Some(dir), Some(stem)) = (dst.parent(), dst.file_stem()) {
                let _ = std::fs::remove_dir_all(dir.join(stem));
            }
            return Err(err);
        }
    }
    Ok(truncated)
}

/// The agents the session `root` at `src` started that its copied history
/// `named`, with every agent those started by `until`, as (session id,
/// file), from the headers in its `agents` directory. An agent of an
/// agent is in that same directory, so a fork of an agent finds its
/// agents there too. An agent of a turn undone or rewound away is named
/// nowhere in the copy, so it stays behind.
///
/// # Errors
///
/// Errors if the directory cannot be read, or the file of an agent the
/// history names cannot, since a fork missing that agent would lose it
/// with the source. Any other file that cannot be read, is deleted
/// meanwhile or has no header yet is no agent to copy.
fn agents_started_by(
    src: &Path,
    root: SessionId,
    named: &HashSet<String>,
    until: chrono::DateTime<Utc>,
) -> Result<Vec<(String, PathBuf)>, SessionError> {
    let dir = crate::layout::agents_dir(src);
    let io = |source| SessionError::Io {
        path: dir.clone(),
        source,
    };
    let mut started: HashMap<String, Vec<(String, chrono::DateTime<Utc>, PathBuf)>> =
        HashMap::new();
    for entry in std::fs::read_dir(&dir).map_err(io)? {
        let path = entry.map_err(io)?.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") || path == src {
            continue;
        }
        let header = match SessionReader::iter(&path).map(|mut reader| reader.next()) {
            Ok(Some(Ok(SessionEntry::Header(header)))) => header,
            Err(err @ SessionError::Io { .. }) | Ok(Some(Err(err @ SessionError::Io { .. })))
                if path.exists()
                    && path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(|id| named.contains(id)) =>
            {
                return Err(err);
            }
            _ => continue,
        };
        if let Some(parent) = header.parent_session {
            started.entry(parent.to_string()).or_default().push((
                header.session.to_string(),
                header.ts,
                path,
            ));
        }
    }
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    let mut pending: Vec<(String, bool)> = vec![(root.to_string(), true)];
    while let Some((parent, is_root)) = pending.pop() {
        for (id, ts, path) in started.get(&parent).into_iter().flatten() {
            let wanted = if is_root {
                named.contains(id)
            } else {
                *ts <= until
            };
            if !wanted || !seen.insert(id.clone()) {
                continue;
            }
            pending.push((id.clone(), false));
            found.push((id.clone(), path.clone()));
        }
    }
    Ok(found)
}

/// Copies each of `agents` into `dir` under its new id from `renames`,
/// with every id `renames` names replaced in its lines, through the last
/// entry written by `until`, the fork point: what an agent did after it
/// happened in the source alone. A torn last line of an agent still
/// being written is left out, and a line that does not decode ends the
/// copy, so the copy reads back whole.
fn copy_agents(
    agents: &[(String, PathBuf)],
    dir: &Path,
    renames: &HashMap<String, String>,
    until: chrono::DateTime<Utc>,
) -> Result<(), SessionError> {
    for (id, path) in agents {
        let io = |source: std::io::Error| SessionError::Io {
            path: path.clone(),
            source,
        };
        let data = std::fs::read(path).map_err(io)?;
        let text = String::from_utf8_lossy(&data);
        let mut lines = text
            .split_inclusive('\n')
            .filter(|line| line.ends_with('\n'));
        let Some(first) = lines.next() else {
            continue;
        };
        let SessionEntry::Header(header) = serde_json::from_str(&rename_ids(first, renames))
            .map_err(|source| SessionError::Encode {
                path: path.clone(),
                source,
            })?
        else {
            return Err(SessionError::MissingHeader { path: path.clone() });
        };
        let rest: String = lines
            .filter(|line| !line.trim_ascii().is_empty())
            .map(|line| rename_ids(line, renames))
            .take_while(|line| {
                serde_json::from_str::<SessionEntry>(line).is_ok_and(|entry| entry.ts() <= until)
            })
            .collect();
        let dst = dir.join(format!("{}.jsonl", renames[id]));
        let mut writer = SessionWriter::create(dst, header)?;
        if !rest.is_empty() {
            writer.append_lines(rest.as_bytes())?;
        }
    }
    Ok(())
}

/// The spans of `text` that read as a session id standing on its own: 26
/// characters of a ULID's alphabet, after a quote, a space, a JSON escape
/// such as `\n` or the start, and before no letter or digit. One that is
/// a path component, after or before `/` or an escaped `\`, names a
/// directory such as an agent's worktree and is left alone.
fn id_spans(text: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    const LEN: usize = 26;
    let bytes = text.as_bytes();
    let starts_alone = move |at: usize| match at.checked_sub(1).map(|before| bytes[before]) {
        None => true,
        Some(b'/' | b'\\') => false,
        Some(b'n' | b'r' | b't') => at >= 2 && bytes[at - 2] == b'\\',
        Some(byte) => !byte.is_ascii_alphanumeric(),
    };
    // A quote inside a JSON string is escaped, so a `\` alone may close
    // an id; an escaped `\` is a path separator.
    let ends_alone = move |end: usize| match bytes.get(end) {
        None => true,
        Some(b'/') => false,
        Some(b'\\') => bytes.get(end + 1) != Some(&b'\\'),
        Some(byte) => !byte.is_ascii_alphanumeric(),
    };
    let mut at = 0;
    std::iter::from_fn(move || {
        while at + LEN <= bytes.len() {
            let start = at;
            at += 1;
            if starts_alone(start)
                && bytes[start..start + LEN]
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
                && ends_alone(start + LEN)
            {
                at = start + LEN;
                return Some(start..start + LEN);
            }
        }
        None
    })
}

/// Every session id [`id_spans`] finds in `text`.
fn ids_in(text: &str) -> impl Iterator<Item = &str> {
    id_spans(text).map(move |span| &text[span])
}

/// `text` with every session id [`id_spans`] finds that `renames` names
/// replaced.
fn rename_ids(text: &str, renames: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for span in id_spans(text) {
        if let Some(new) = renames.get(&text[span.clone()]) {
            out.push_str(&text[copied..span.start]);
            out.push_str(new);
            copied = span.end;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// A session's entries after its header, up to and including the last
/// one a filter kept, as the raw lines of the file.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The last entry kept, or the header when no entry was.
    pub at: EntryId,
    /// The entry lines up to `at`, each ending in `\n`.
    pub lines: Vec<u8>,
    /// Whether the source's tail was torn, so entries after `at` may
    /// have been lost.
    pub truncated: bool,
    /// The blob store of the source's directory.
    pub blobs: BlobStore,
    /// The blobs named by entries copied through the last kept entry.
    pub blob_refs: Vec<String>,
    source_header: EntryId,
}

impl Snapshot {
    /// Whether any entry past the header was kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Create the session file `dst` under `header` holding these
    /// entries, with one fsync for all of them, and keep it open for
    /// appending. When `dst` is in another directory, the blobs the
    /// entries name are copied into its store. A supported older header
    /// version is upgraded to the current format version in the destination.
    /// Branches and labels targeting the source header are retargeted to
    /// `header`.
    ///
    /// # Errors
    ///
    /// Errors if `dst` already exists or cannot be written, or if the
    /// destination header names an external backend, has an unsupported version,
    /// or has an ID that collides with a copied entry.
    pub fn write(&self, dst: &Path, header: Header) -> Result<SessionWriter, SessionError> {
        if header.backend.is_some() {
            return Err(SessionError::ExternalFork {
                path: dst.to_path_buf(),
            });
        }
        ensure_supported_version(dst, header.version)?;
        let mut rewritten = Vec::new();
        for line in self.lines.split_inclusive(|byte| *byte == b'\n') {
            let entry = serde_json::from_slice::<SessionEntry>(line).ok();
            if entry.as_ref().is_some_and(|entry| entry.id() == header.id) {
                return Err(SessionError::Io {
                    path: dst.to_path_buf(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "destination header ID collides with a copied entry",
                    ),
                });
            }
            let remapped = entry
                .as_ref()
                .and_then(|entry| remap_root(entry, self.source_header, header.id));
            if let Some(entry) = remapped {
                serde_json::to_writer(&mut rewritten, &entry).map_err(|source| {
                    SessionError::Encode {
                        path: dst.to_path_buf(),
                        source,
                    }
                })?;
                if line.ends_with(b"\n") {
                    rewritten.push(b'\n');
                }
            } else {
                rewritten.extend_from_slice(line);
            }
        }
        let target = BlobStore::beside(dst);
        if target != self.blobs {
            for sha256 in &self.blob_refs {
                if let Some(bytes) = self.blobs.get(sha256) {
                    target.put(&bytes).map_err(|err| SessionError::Io {
                        path: dst.to_path_buf(),
                        source: err,
                    })?;
                }
            }
        }
        let mut writer = SessionWriter::create(PathBuf::from(dst), header)?;
        if !self.is_empty() {
            writer.append_lines(&rewritten)?;
        }
        Ok(writer)
    }
}

/// Read `src` once and keep its entries up to the last one `keep`
/// accepts. An unterminated trailing fragment that ends prematurely is
/// treated as a torn write, and the snapshot's `truncated` flag reports it.
/// Malformed JSON and complete JSON that does not match the entry schema
/// are errors.
///
/// # Errors
///
/// Errors if `src` cannot be read, if its first entry is not a header,
/// if the source has an unsupported version or an external backend, or
/// if a second header appears, or if a complete JSON value or terminated
/// line does not decode as an entry.
pub fn snapshot(
    src: &Path,
    keep: impl Fn(&SessionEntry) -> bool,
) -> Result<Snapshot, SessionError> {
    let data = std::fs::read(src).map_err(|err| SessionError::Io {
        path: src.to_path_buf(),
        source: err,
    })?;
    let mut lines = data.split_inclusive(|byte| *byte == b'\n');
    let mut offset = 0;
    let mut line_no = 0;
    let header_id = loop {
        let Some(line) = lines.next() else {
            return Err(SessionError::Empty {
                path: src.to_path_buf(),
            });
        };
        offset += line.len();
        line_no += 1;
        if line.trim_ascii().is_empty() {
            continue;
        }
        match serde_json::from_slice::<SessionEntry>(line) {
            Ok(SessionEntry::Header(header)) => {
                ensure_supported_version(src, header.version)?;
                if header.backend.is_some() {
                    return Err(SessionError::ExternalFork {
                        path: src.to_path_buf(),
                    });
                }
                break header.id;
            }
            _ => {
                return Err(SessionError::MissingHeader {
                    path: src.to_path_buf(),
                });
            }
        }
    };
    let start = offset;
    let mut kept = (header_id, start);
    let mut refs: Vec<(usize, String)> = Vec::new();
    let mut truncated = false;
    for line in lines {
        line_no += 1;
        offset += line.len();
        match crate::reader::classify_line(line, line.ends_with(b"\n")) {
            crate::reader::Line::Entry(SessionEntry::Header(_)) => {
                return Err(SessionError::SecondHeader {
                    path: src.to_path_buf(),
                });
            }
            crate::reader::Line::Entry(entry) => {
                if keep(&entry) {
                    kept = (entry.id(), offset);
                }
                refs.extend(blob_refs(&entry).into_iter().map(|sha| (offset, sha)));
            }
            crate::reader::Line::Blank => {}
            crate::reader::Line::Torn => {
                truncated = true;
                break;
            }
            crate::reader::Line::Decode(source) => {
                return Err(SessionError::Decode {
                    path: src.to_path_buf(),
                    line: line_no,
                    source,
                });
            }
            crate::reader::Line::InvalidUtf8(source) => {
                return Err(SessionError::Io {
                    path: src.to_path_buf(),
                    source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
                });
            }
        }
    }
    let (at, end) = kept;
    let mut lines = data[start..end].to_vec();
    if lines.last().is_some_and(|byte| *byte != b'\n') {
        lines.push(b'\n');
    }
    Ok(Snapshot {
        at,
        lines,
        truncated,
        blobs: BlobStore::beside(src),
        source_header: header_id,
        blob_refs: refs
            .into_iter()
            .filter(|(at, _)| *at <= end)
            .map(|(_, sha)| sha)
            .collect(),
    })
}

fn remap_root(entry: &SessionEntry, source: EntryId, destination: EntryId) -> Option<SessionEntry> {
    match entry {
        SessionEntry::Branch(branch) if branch.at == source => {
            let mut branch = branch.clone();
            branch.at = destination;
            Some(SessionEntry::Branch(branch))
        }
        SessionEntry::Label(label) if label.anchor == source => {
            let mut label = label.clone();
            label.anchor = destination;
            Some(SessionEntry::Label(label))
        }
        _ => None,
    }
}

fn ensure_supported_version(path: &Path, version: u32) -> Result<(), SessionError> {
    if version == 0 || version > FORMAT_VERSION {
        return Err(SessionError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    Ok(())
}

/// The blobs a message entry names.
fn blob_refs(entry: &SessionEntry) -> Vec<String> {
    let SessionEntry::Message(message) = entry else {
        return Vec::new();
    };
    message
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            kage_core::Content::Image {
                source: kage_core::MediaSource::Blob { sha256, .. },
                ..
            }
            | kage_core::Content::Audio {
                source: kage_core::MediaSource::Blob { sha256, .. },
                ..
            } => Some(sha256.clone()),
            _ => None,
        })
        .collect()
}

/// Resolve `prefix` against entry ids in `src`. Errors if zero or multiple
/// entries match.
pub fn resolve_entry_prefix(src: &Path, prefix: &str) -> Result<EntryId, SessionError> {
    let reader = SessionReader::iter(src)?;
    let mut matches: Vec<EntryId> = Vec::new();
    for item in reader {
        let entry = item?;
        let id = entry.id();
        if id.to_string().starts_with(prefix) {
            matches.push(id);
        }
    }
    if matches.is_empty() {
        return Err(SessionError::Io {
            path: src.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no entry id starts with '{prefix}'"),
            ),
        });
    }
    if matches.len() > 1 {
        return Err(SessionError::Io {
            path: src.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "entry id prefix '{prefix}' is ambiguous ({} matches)",
                    matches.len()
                ),
            ),
        });
    }
    Ok(matches.remove(0))
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
        BranchEntry, EntryId, FORMAT_VERSION, Header, Label, MessageEntry, SessionEntry, SessionId,
    };
    use crate::reader::SessionReader;
    use crate::writer::SessionWriter;

    fn fresh_header(model: &str) -> Header {
        Header {
            version: FORMAT_VERSION,
            session: SessionId::new(),
            id: EntryId::new(),
            ts: Utc::now(),
            cwd: PathBuf::from("/work"),
            model: model.into(),
            system_prompt: "be helpful".into(),
            parent_session: None,
            backend: None,
            parent_entry: None,
        }
    }

    fn message_entry(role: Role, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            message: Arc::new(Message::new(
                role,
                vec![Content::Text {
                    text: text.to_owned(),
                }],
                None,
            )),
            usage: None,
        })
    }

    fn write(path: &Path, header: Header, entries: &[SessionEntry]) {
        let mut w = SessionWriter::create(path, header).unwrap();
        for e in entries {
            w.append(e).unwrap();
        }
    }

    #[test]
    fn fork_as_rejects_header_id_matching_copied_entry_before_creation() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let dst = dir.path().join("dst.jsonl");
        let entry = message_entry(Role::User, "one");
        write(&src, fresh_header("x:y"), std::slice::from_ref(&entry));
        let mut header = fresh_header("x:y");
        header.id = entry.id();
        assert!(matches!(
            fork_as(&src, &dst, header, entry.id()),
            Err(SessionError::Io { source, .. }) if source.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(!dst.exists());
        let mut header = fresh_header("x:y");
        header.id = entry.id();
        let root = SessionTree::read(&src).unwrap().header().id;
        fork_as(&src, &dst, header, root).unwrap();
        assert!(SessionTree::read(&dst).is_ok());
    }

    #[test]
    fn snapshot_rejects_header_id_matching_copied_entry_before_creation() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let dst = dir.path().join("dst.jsonl");
        let entry = message_entry(Role::User, "one");
        write(&src, fresh_header("x:y"), std::slice::from_ref(&entry));
        let snapshot = snapshot(&src, |_| true).unwrap();
        let mut header = fresh_header("x:y");
        header.id = entry.id();
        assert!(matches!(
            snapshot.write(&dst, header),
            Err(SessionError::Io { source, .. }) if source.kind() == std::io::ErrorKind::InvalidInput
        ));
        assert!(!dst.exists());
    }

    #[test]
    fn snapshot_reports_corrupt_tails_and_preserves_incomplete_tails() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let header = serde_json::to_vec(&SessionEntry::Header(fresh_header("x:y"))).unwrap();
        for tail in [b"{bad}".as_slice(), b"{bad}\n", b"{}", b"{}\n", b"\xff"] {
            let mut data = header.clone();
            data.push(b'\n');
            data.extend_from_slice(tail);
            std::fs::write(&src, data).unwrap();
            assert!(snapshot(&src, |_| true).is_err(), "tail {tail:?}");
        }
        let mut data = b"\n".to_vec();
        data.extend_from_slice(&header);
        data.extend_from_slice(b"\n{bad}\n");
        std::fs::write(&src, &data).unwrap();
        assert!(matches!(
            snapshot(&src, |_| true),
            Err(SessionError::Decode { line: 3, .. })
        ));
        data = header;
        data.extend_from_slice(b"\n{\"type\":");
        std::fs::write(&src, data).unwrap();
        assert!(snapshot(&src, |_| true).unwrap().truncated);
    }

    #[test]
    fn a_snapshot_keeps_through_the_last_kept_entry_and_skips_a_torn_tail() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let header = fresh_header("x:y");
        let header_id = header.id;
        let entries = [
            message_entry(Role::User, "one"),
            message_entry(Role::Assistant, "two"),
            message_entry(Role::User, "three"),
        ];
        write(&src, header, &entries);
        let mut file = std::fs::OpenOptions::new().append(true).open(&src).unwrap();
        std::io::Write::write_all(&mut file, b"{\"type\":\"mess").unwrap();

        let is_assistant = |entry: &SessionEntry| matches!(entry, SessionEntry::Message(m) if m.message.role == Role::Assistant);
        let snap = snapshot(&src, is_assistant).unwrap();
        assert_eq!(snap.at, entries[1].id());
        assert!(snap.truncated, "the torn tail is reported");
        let dst = dir.path().join("dst.jsonl");
        drop(snap.write(&dst, fresh_header("x:y")).unwrap());
        let copied: Vec<EntryId> = SessionReader::iter(&dst)
            .unwrap()
            .skip(1)
            .map(|e| e.unwrap().id())
            .collect();
        assert_eq!(copied, [entries[0].id(), entries[1].id()]);

        let none = snapshot(&src, |_| false).unwrap();
        assert!(none.is_empty());
        assert_eq!(none.at, header_id);
        assert!(none.truncated);
    }

    #[test]
    fn snapshot_rejects_complete_corrupt_lines_and_reports_physical_line() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        write(&src, fresh_header("x:y"), &[]);
        let header = std::fs::read(&src).unwrap();
        let mut data = b"\n".to_vec();
        data.extend_from_slice(&header);
        data.extend_from_slice(b"{broken}\n");
        std::fs::write(&src, &data).unwrap();
        assert!(matches!(
            snapshot(&src, |_| true),
            Err(SessionError::Decode { path, line: 3, .. }) if path == src
        ));
        data.pop();
        std::fs::write(&src, data).unwrap();
        assert!(matches!(
            snapshot(&src, |_| true),
            Err(SessionError::Decode { path, line: 3, .. }) if path == src
        ));
    }

    #[test]
    fn snapshot_rejects_second_headers_even_after_last_kept_entry() {
        use crate::entry::{ExternalAcpSession, SessionBackend};

        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let mut external = fresh_header("external");
        external.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "agent".into(),
            protocol_version: 1,
            upstream_session_id: "upstream".into(),
        }));
        for header in [fresh_header("native"), external] {
            for terminated in [true, false] {
                write(
                    &src,
                    fresh_header("native"),
                    &[message_entry(Role::User, "hello")],
                );
                let mut data = std::fs::read(&src).unwrap();
                data.extend_from_slice(
                    &serde_json::to_vec(&SessionEntry::Header(header.clone())).unwrap(),
                );
                if terminated {
                    data.push(b'\n');
                }
                std::fs::write(&src, data).unwrap();
                assert!(
                    matches!(snapshot(&src, |_| false), Err(SessionError::SecondHeader { path }) if path == src)
                );
                std::fs::remove_file(&src).unwrap();
            }
        }
    }

    #[test]
    fn snapshot_rejects_malformed_unterminated_json_but_reports_incomplete_json() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        for (tail, torn) in [
            (b"{\"type\":]".as_slice(), false),
            (b"{\"type\":\"message\"", true),
        ] {
            write(&src, fresh_header("native"), &[]);
            let mut data = std::fs::read(&src).unwrap();
            data.extend_from_slice(tail);
            std::fs::write(&src, data).unwrap();
            if torn {
                assert!(snapshot(&src, |_| true).unwrap().truncated);
            } else {
                assert!(
                    matches!(snapshot(&src, |_| true), Err(SessionError::Decode { path, line: 2, .. }) if path == src)
                );
            }
            std::fs::remove_file(&src).unwrap();
        }
    }

    #[test]
    fn snapshot_remaps_root_branches_to_destination_header() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let header = fresh_header("native");
        let first = message_entry(Role::User, "abandoned");
        let root_branch = SessionEntry::Branch(BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: header.id,
        });
        let second = message_entry(Role::User, "active");
        let normal_branch = SessionEntry::Branch(BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: second.id(),
        });
        let last = message_entry(Role::Assistant, "reply");
        write(
            &src,
            header,
            &[
                first.clone(),
                root_branch.clone(),
                second.clone(),
                normal_branch.clone(),
                last.clone(),
            ],
        );
        let snap = snapshot(&src, |_| true).unwrap();
        let dst = dir.path().join("dst.jsonl");
        let new_header = fresh_header("child");
        let new_id = new_header.id;
        drop(snap.write(&dst, new_header).unwrap());
        let entries: Vec<_> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            matches!(&entries[2], SessionEntry::Branch(branch) if branch.at == new_id && branch.id == root_branch.id())
        );
        assert_eq!(entries[4], normal_branch);
        assert_eq!(entries[1], first);
        assert_eq!(entries[3], second);
        assert_eq!(entries[5], last);
        let tree = SessionTree::read(&dst).unwrap();
        assert_eq!(
            tree.ancestry(last.id())
                .iter()
                .map(|entry| entry.id())
                .collect::<Vec<_>>(),
            vec![second.id(), last.id()]
        );
    }

    #[test]
    fn fork_and_snapshot_remap_root_label_anchors_but_preserve_lineage() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let header = fresh_header("native");
        let source_root = header.id;
        let source_session = header.session;
        let message = message_entry(Role::User, "message");
        let root_label = SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "root".into(),
            anchor: source_root,
        });
        let normal_label = SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "message".into(),
            anchor: message.id(),
        });
        write(
            &src,
            header,
            &[message.clone(), root_label.clone(), normal_label.clone()],
        );
        let snap = snapshot(&src, |_| true).unwrap();
        for use_snapshot in [false, true] {
            let dst = dir.path().join(if use_snapshot {
                "snapshot.jsonl"
            } else {
                "fork.jsonl"
            });
            let mut child = fresh_header("child");
            child.parent_session = Some(source_session);
            child.parent_entry = Some(source_root);
            let child_root = child.id;
            if use_snapshot {
                drop(snap.write(&dst, child).unwrap());
            } else {
                fork_as(&src, &dst, child, normal_label.id()).unwrap();
            }
            let entries: Vec<_> = SessionReader::iter(&dst)
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(
                matches!(&entries[0], SessionEntry::Header(h) if h.id == child_root && h.parent_session == Some(source_session) && h.parent_entry == Some(source_root))
            );
            assert!(
                matches!(&entries[2], SessionEntry::Label(label) if label.id == root_label.id() && label.anchor == child_root)
            );
            assert_eq!(entries[3], normal_label);
            assert!(SessionTree::read(&dst).is_ok());
        }
    }

    #[test]
    fn snapshot_rejects_complete_schema_invalid_unterminated_json() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        for tail in [b"{}".as_slice(), b"{\"type\":\"message\"}", b"null"] {
            write(&src, fresh_header("native"), &[]);
            let mut data = std::fs::read(&src).unwrap();
            data.extend_from_slice(tail);
            std::fs::write(&src, data).unwrap();
            assert!(
                matches!(snapshot(&src, |_| true), Err(SessionError::Decode { path, line: 2, .. }) if path == src)
            );
            std::fs::remove_file(&src).unwrap();
        }
    }

    #[test]
    fn invalid_destination_versions_create_no_files_or_blobs() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let source_header = fresh_header("native");
        let at = source_header.id;
        write(&src, source_header, &[]);
        let mut snap = snapshot(&src, |_| true).unwrap();
        snap.blob_refs.push(snap.blobs.put(b"blob").unwrap());
        for version in [0, FORMAT_VERSION + 1] {
            let mut header = fresh_header("native");
            header.version = version;
            let dst = dir.path().join(format!("missing-{version}/dst.jsonl"));
            assert!(
                matches!(snap.write(&dst, header.clone()), Err(SessionError::UnsupportedVersion { path, found, supported }) if path == dst && found == version && supported == FORMAT_VERSION)
            );
            assert!(!dst.parent().unwrap().exists());
            assert!(
                matches!(fork_as(&src, &dst, header, at), Err(SessionError::UnsupportedVersion { path, found, supported }) if path == dst && found == version && supported == FORMAT_VERSION)
            );
            assert!(!dst.parent().unwrap().exists());
        }
    }

    #[test]
    fn older_destination_headers_are_upgraded_when_copying_entries() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let entry = message_entry(Role::User, "copied content");
        write(&src, fresh_header("native"), std::slice::from_ref(&entry));
        let snap = snapshot(&src, |_| true).unwrap();

        for use_snapshot in [false, true] {
            let dst = dir.path().join(if use_snapshot {
                "snapshot.jsonl"
            } else {
                "fork.jsonl"
            });
            let mut header = fresh_header("child");
            header.version = 1;
            let header_id = header.id;
            if use_snapshot {
                drop(snap.write(&dst, header).unwrap());
            } else {
                assert!(!fork_as(&src, &dst, header, entry.id()).unwrap());
            }
            let mut reader = SessionReader::iter(&dst).unwrap();
            let SessionEntry::Header(copied_header) = reader.next().unwrap().unwrap() else {
                panic!("expected header");
            };
            assert_eq!(copied_header.id, header_id);
            assert_eq!(copied_header.version, FORMAT_VERSION);
            let copied = reader.next().unwrap().unwrap();
            assert_eq!(
                serde_json::to_value(copied).unwrap(),
                serde_json::to_value(&entry).unwrap()
            );
            assert!(reader.next().is_none());
        }
    }

    #[test]
    fn snapshot_rejects_unsupported_versions_before_filtering() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        for version in [0, FORMAT_VERSION + 1] {
            let mut header = fresh_header("x:y");
            header.version = version;
            let mut data = serde_json::to_vec(&SessionEntry::Header(header)).unwrap();
            data.push(b'\n');
            data.extend(serde_json::to_vec(&message_entry(Role::User, "hello")).unwrap());
            data.push(b'\n');
            std::fs::write(&src, data).unwrap();
            assert!(matches!(
                snapshot(&src, |_| panic!("unsupported entries must not reach the filter")),
                Err(SessionError::UnsupportedVersion { path, found, supported })
                    if path == src && found == version && supported == FORMAT_VERSION
            ));
            std::fs::remove_file(&src).unwrap();
        }
    }

    #[test]
    fn snapshot_copies_blobs_of_rejected_entries_before_last_kept_entry() {
        use kage_core::MediaSource;

        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let dst = dir.path().join("other/dst.jsonl");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        let blobs = BlobStore::beside(&src);
        let bytes = b"image contents";
        let sha = blobs.put(bytes).unwrap();
        let later_sha = blobs.put(b"not copied").unwrap();
        let image = |sha256: String| {
            SessionEntry::Message(MessageEntry {
                id: EntryId::new(),
                ts: Utc::now(),
                message: Arc::new(Message::new(
                    Role::User,
                    vec![Content::Image {
                        mime: "image/png".into(),
                        source: MediaSource::Blob {
                            sha256,
                            size: bytes.len() as u64,
                        },
                    }],
                    None,
                )),
                usage: None,
            })
        };
        let first = image(sha.clone());
        let kept = message_entry(Role::Assistant, "keep");
        let last = image(later_sha.clone());
        write(
            &src,
            fresh_header("x:y"),
            &[first.clone(), kept.clone(), last],
        );
        let snap = snapshot(&src, |entry| entry.id() == kept.id()).unwrap();
        assert_eq!(snap.at, kept.id());
        assert_eq!(snap.blob_refs, vec![sha.clone()]);
        drop(snap.write(&dst, fresh_header("x:y")).unwrap());
        let target = BlobStore::beside(&dst);
        assert_eq!(target.get(&sha).as_deref(), Some(bytes.as_slice()));
        assert!(target.get(&later_sha).is_none());
        let copied: Vec<_> = SessionReader::iter(&dst)
            .unwrap()
            .skip(1)
            .map(|entry| entry.unwrap().id())
            .collect();
        assert_eq!(copied, [first.id(), kept.id()]);
    }

    #[test]
    fn snapshots_refuse_external_sources_and_destinations_before_writing() {
        use crate::entry::{ExternalAcpSession, SessionBackend};

        let dir = tempdir().unwrap();
        let src = dir.path().join("external.jsonl");
        let mut external = fresh_header("external");
        external.cwd = dir.path().to_path_buf();
        external.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "agent".into(),
            protocol_version: 1,
            upstream_session_id: "upstream".into(),
        }));
        write(
            &src,
            external.clone(),
            &[message_entry(Role::User, "secret")],
        );
        assert!(matches!(
            snapshot(&src, |_| panic!("external entries must not reach the filter")),
            Err(SessionError::ExternalFork { path }) if path == src
        ));

        let native = dir.path().join("native.jsonl");
        write(
            &native,
            fresh_header("native"),
            &[message_entry(Role::User, "hello")],
        );
        let mut snap = snapshot(&native, |_| true).unwrap();
        let sha = snap.blobs.put(b"blob contents").unwrap();
        snap.blob_refs.push(sha);
        let dst = dir.path().join("missing/child.jsonl");
        assert!(matches!(
            snap.write(&dst, external),
            Err(SessionError::ExternalFork { path }) if path == dst
        ));
        assert!(!dst.parent().unwrap().exists());
    }

    #[test]
    fn a_whole_snapshot_reports_no_truncation() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let m1 = message_entry(Role::User, "one");
        write(&src, fresh_header("x:y"), std::slice::from_ref(&m1));
        let snap = snapshot(&src, |_| true).unwrap();
        assert!(!snap.truncated);
    }

    #[test]
    fn a_fork_from_a_torn_source_reports_the_truncation() {
        let dir = tempdir().unwrap();
        let torn = dir.path().join("torn.jsonl");
        let whole = dir.path().join("whole.jsonl");
        let m1 = message_entry(Role::User, "one");
        let m2 = message_entry(Role::Assistant, "two");
        let m3 = message_entry(Role::User, "three");
        write(
            &torn,
            fresh_header("x:y"),
            &[m1.clone(), m2.clone(), m3.clone()],
        );
        write(&whole, fresh_header("x:y"), &[m1, m2.clone(), m3]);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&torn)
            .unwrap();
        std::io::Write::write_all(&mut file, b"{\"type\":\"mess").unwrap();

        let torn_id = SessionId::new();
        let torn_at = m2.id();
        assert!(
            fork(&torn, &dir.path().join("torn-fork.jsonl"), torn_id, torn_at).unwrap(),
            "the torn tail is reported"
        );
        let whole_id = SessionId::new();
        assert!(
            !fork(
                &whole,
                &dir.path().join("whole-fork.jsonl"),
                whole_id,
                torn_at
            )
            .unwrap(),
            "an intact source is not truncated"
        );
    }

    #[test]
    fn generic_forks_refuse_external_identity_without_creating_destination() {
        use crate::entry::{ExternalAcpSession, SessionBackend};
        let dir = tempdir().unwrap();
        let src = dir.path().join("source.jsonl");
        let mut external = fresh_header("external");
        external.cwd = dir.path().to_path_buf();
        external.backend = Some(SessionBackend::ExternalAcp(ExternalAcpSession {
            agent_name: "agent".into(),
            protocol_version: 1,
            upstream_session_id: "upstream".into(),
        }));
        let at = external.id;
        write(&src, external.clone(), &[]);
        let dst = dir.path().join("missing/child.jsonl");
        assert!(matches!(
            fork(&src, &dst, SessionId::new(), at),
            Err(SessionError::ExternalFork { .. })
        ));
        assert!(matches!(
            fork_as(&src, &dst, fresh_header("native"), at),
            Err(SessionError::ExternalFork { .. })
        ));
        assert!(!dst.parent().unwrap().exists());

        let native = dir.path().join("native.jsonl");
        let header = fresh_header("native");
        let native_at = header.id;
        write(&native, header, &[]);
        assert!(matches!(
            fork_as(&native, &dst, external, native_at),
            Err(SessionError::ExternalFork { .. })
        ));
        assert!(!dst.parent().unwrap().exists());
    }

    #[test]
    fn fork_copies_through_target_entry() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let parent_header = fresh_header("anthropic:claude");
        let parent_id = parent_header.session;
        let m1 = message_entry(Role::User, "first");
        let m1_id = m1.id();
        let m2 = message_entry(Role::Assistant, "second");
        let m3 = message_entry(Role::User, "third");
        write(&src, parent_header, &[m1.clone(), m2.clone(), m3.clone()]);

        let dst = dir.path().join("forked.jsonl");
        let new_id = SessionId::new();
        fork(&src, &dst, new_id, m1_id).unwrap();

        let entries: Vec<_> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Header + m1 only.
        assert_eq!(entries.len(), 2);
        let SessionEntry::Header(h) = &entries[0] else {
            panic!("expected header");
        };
        assert_eq!(h.session, new_id);
        assert_eq!(h.parent_session, Some(parent_id));
        assert_eq!(h.parent_entry, Some(m1_id));
        assert_eq!(h.model, "anthropic:claude");
        let SessionEntry::Message(m) = &entries[1] else {
            panic!("expected message");
        };
        match &m.message.content[0] {
            Content::Text { text } => assert_eq!(text, "first"),
            other => panic!("unexpected content: {other:?}"),
        }
    }

    #[test]
    fn fork_at_header_copies_only_header() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let parent_header = fresh_header("openai:gpt");
        let header_entry_id = parent_header.id;
        write(&src, parent_header, &[message_entry(Role::User, "hello")]);

        let dst = dir.path().join("forked.jsonl");
        fork(&src, &dst, SessionId::new(), header_entry_id).unwrap();

        let entries: Vec<_> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], SessionEntry::Header(_)));
    }

    #[test]
    fn fork_at_last_entry_copies_everything() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let parent_header = fresh_header("openai:gpt");
        let m1 = message_entry(Role::User, "a");
        let m2 = message_entry(Role::Assistant, "b");
        let last_id = m2.id();
        write(&src, parent_header, &[m1.clone(), m2.clone()]);

        let dst = dir.path().join("forked.jsonl");
        fork(&src, &dst, SessionId::new(), last_id).unwrap();
        let entries: Vec<_> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Header + m1 + m2.
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn fork_with_unknown_entry_id_errors_and_cleans_up() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        write(
            &src,
            fresh_header("x:y"),
            &[message_entry(Role::User, "only one")],
        );

        let dst = dir.path().join("forked.jsonl");
        let err = fork(&src, &dst, SessionId::new(), EntryId::new()).unwrap_err();
        assert!(matches!(err, SessionError::EntryNotFound { .. }), "{err:?}");
        assert!(err.to_string().contains("has no entry"), "{err}");
        assert!(!dst.exists(), "fork should clean up its dst on error");
    }

    #[test]
    fn fork_refuses_to_overwrite() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let hi = message_entry(Role::User, "hi");
        write(&src, fresh_header("x:y"), std::slice::from_ref(&hi));

        let dst = dir.path().join("dst.jsonl");
        std::fs::write(&dst, b"existing").unwrap();
        let err = fork(&src, &dst, SessionId::new(), hi.id()).unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }

    #[test]
    fn resolve_entry_prefix_finds_unique() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("s.jsonl");
        let m1 = message_entry(Role::User, "x");
        let id_str = m1.id().to_string();
        // Ulids generated in the same millisecond share a long prefix, so
        // we look up the entry by its full id here.
        write(&src, fresh_header("x:y"), std::slice::from_ref(&m1));
        let resolved = resolve_entry_prefix(&src, &id_str).unwrap();
        assert_eq!(resolved.to_string(), id_str);
    }

    #[test]
    fn resolve_entry_prefix_errors_on_no_match() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("s.jsonl");
        write(&src, fresh_header("x:y"), &[message_entry(Role::User, "x")]);
        let err = resolve_entry_prefix(&src, "ZZZZZZZZZZ").unwrap_err();
        assert!(matches!(err, SessionError::Io { .. }));
    }

    #[test]
    fn resolve_entry_prefix_rejects_an_ambiguous_prefix() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("s.jsonl");
        // Two ids crafted to share a long prefix: the Crockford base32
        // encodings differ only in their last characters.
        let first = EntryId(ulid::Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap());
        let second = EntryId(ulid::Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap());
        assert_ne!(first, second);
        let shared = &first.to_string()[..20];
        assert!(
            second.to_string().starts_with(shared),
            "fixture ids share {shared}"
        );
        let a = SessionEntry::Message(MessageEntry {
            id: first,
            ts: Utc::now(),
            message: Arc::new(Message::new(
                Role::User,
                vec![Content::Text { text: "a".into() }],
                None,
            )),
            usage: None,
        });
        let b = SessionEntry::Message(MessageEntry {
            id: second,
            ts: Utc::now(),
            message: Arc::new(Message::new(
                Role::User,
                vec![Content::Text { text: "b".into() }],
                None,
            )),
            usage: None,
        });
        write(&src, fresh_header("x:y"), &[a, b]);

        let err = resolve_entry_prefix(&src, shared).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err}");
    }

    #[test]
    fn fork_copies_labels() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let m1 = message_entry(Role::User, "one");
        let anchor = m1.id();
        let label = SessionEntry::Label(Label {
            id: EntryId::new(),
            ts: Utc::now(),
            text: "milestone".to_owned(),
            anchor,
        });
        let m2 = message_entry(Role::Assistant, "two");
        write(&src, fresh_header("x:y"), &[m1, label, m2.clone()]);

        let dst = dir.path().join("forked.jsonl");
        fork(&src, &dst, SessionId::new(), m2.id()).unwrap();
        let copied: Vec<SessionEntry> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // Header + m1 + label + m2: the label is its own entry, so a
        // fork through m2 carries four lines.
        assert_eq!(
            copied.len(),
            4,
            "header, the labeled message, the label and m2"
        );
        assert!(
            matches!(&copied[2], SessionEntry::Label(l) if l.text == "milestone" && l.anchor == anchor),
            "the label survives with its anchor: {:?}",
            copied[2]
        );
    }

    #[test]
    fn fork_from_a_branched_source_copies_the_active_ancestry() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let header = fresh_header("x:y");
        let kept = message_entry(Role::User, "kept");
        let reply = message_entry(Role::Assistant, "reply");
        let dropped = message_entry(Role::User, "dropped");
        let marker = SessionEntry::Branch(BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: reply.id(),
        });
        let redo = message_entry(Role::User, "redo");
        write(
            &src,
            header.clone(),
            &[kept, reply, dropped, marker, redo.clone()],
        );

        // Fork at the redone prompt: the copy carries kept and reply,
        // not the marker or the abandoned prompt.
        let dst = dir.path().join("forked.jsonl");
        fork(&src, &dst, SessionId::new(), redo.id()).unwrap();
        let copied: Vec<SessionEntry> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let texts: Vec<&str> = copied
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(m) => match &m.message.content[0] {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["kept", "reply", "redo"]);
        assert!(
            copied
                .iter()
                .all(|entry| !matches!(entry, SessionEntry::Branch(_))),
            "a forked child is linear"
        );
    }

    #[test]
    fn fork_at_an_abandoned_entry_copies_that_branch() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let header = fresh_header("x:y");
        let kept = message_entry(Role::User, "kept");
        let reply = message_entry(Role::Assistant, "reply");
        let dropped = message_entry(Role::User, "dropped");
        let dropped_entry = dropped.id();
        let marker = SessionEntry::Branch(BranchEntry {
            id: EntryId::new(),
            ts: Utc::now(),
            at: reply.id(),
        });
        write(&src, header, &[kept, reply, dropped, marker]);

        let dst = dir.path().join("forked.jsonl");
        fork(&src, &dst, SessionId::new(), dropped_entry).unwrap();
        let copied: Vec<SessionEntry> = SessionReader::iter(&dst)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            copied.len(),
            4,
            "header, kept, reply and the abandoned dropped prompt"
        );
    }

    /// The session files directly in `dir`.
    fn jsonl_in(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect()
    }

    /// An agent of `parent` in `agents` with no entries, started
    /// `hours_later` hours from now.
    fn header_only_agent(agents: &Path, parent: SessionId, hours_later: i64) -> SessionId {
        let mut header = fresh_header("m");
        header.parent_session = Some(parent);
        header.ts = Utc::now() + chrono::Duration::hours(hours_later);
        let id = header.session;
        write(&agents.join(format!("{id}.jsonl")), header, &[]);
        id
    }

    fn header_of(path: &Path) -> Header {
        match SessionReader::iter(path).unwrap().next() {
            Some(Ok(SessionEntry::Header(header))) => header,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_fork_owns_copies_of_the_agents_started_by_its_fork_point() {
        let dir = tempdir().unwrap();
        let project = dir.path().join("kage-personal-0123456789");
        std::fs::create_dir_all(&project).unwrap();
        let root = fresh_header("m");
        let root_id = root.session;
        let src = project.join(format!("{root_id}.jsonl"));
        let agents = crate::layout::agents_dir(&src);
        std::fs::create_dir_all(&agents).unwrap();

        let mut kid = fresh_header("m");
        kid.parent_session = Some(root_id);
        let kid_id = kid.session;
        let grandkid_id = header_only_agent(&agents, kid_id, 0);
        let mut after = message_entry(Role::User, "said after the fork point");
        if let SessionEntry::Message(message) = &mut after {
            message.ts = Utc::now() + chrono::Duration::hours(1);
        }
        write(
            &agents.join(format!("{kid_id}.jsonl")),
            kid,
            &[
                message_entry(
                    Role::User,
                    &format!(
                        "<message from=\"kage\" session=\"{root_id}\">go</message> started {grandkid_id}"
                    ),
                ),
                after,
            ],
        );
        header_only_agent(&agents, root_id, 0);
        let reply = message_entry(
            Role::Assistant,
            &format!("<agent session=\"{kid_id}\">done</agent>"),
        );
        let at = reply.id();
        write(&src, root, &[message_entry(Role::User, "hi"), reply]);
        header_only_agent(&agents, root_id, 1);
        header_only_agent(&agents, kid_id, 1);

        let fork_id = SessionId::new();
        let dst = project.join(format!("{fork_id}.jsonl"));
        fork(&src, &dst, fork_id, at).unwrap();

        let copied: Vec<PathBuf> = jsonl_in(&crate::layout::agents_dir(&dst));
        assert_eq!(
            copied.len(),
            2,
            "the kid and its own agent; not an agent the copy never names, nor one started after the fork point"
        );
        let fork_text = std::fs::read_to_string(&dst).unwrap();
        assert!(
            !fork_text.contains(&kid_id.to_string()),
            "the fork names the copy"
        );
        let copy_of_kid = copied
            .iter()
            .find(|path| header_of(path).parent_session == Some(fork_id))
            .expect("the kid's copy belongs to the fork");
        let new_kid = header_of(copy_of_kid).session;
        assert!(fork_text.contains(&new_kid.to_string()));
        assert_eq!(
            copy_of_kid.file_stem().unwrap(),
            new_kid.to_string().as_str()
        );
        let kid_text = std::fs::read_to_string(copy_of_kid).unwrap();
        assert!(
            kid_text.contains(&format!("session=\\\"{fork_id}\\\"")),
            "{kid_text}"
        );
        assert!(
            !kid_text.contains("said after the fork point"),
            "what the agent did after the fork point stays with the source"
        );
        assert!(!kid_text.contains(&grandkid_id.to_string()));
        let copy_of_grandkid = copied.iter().find(|path| *path != copy_of_kid).unwrap();
        assert_eq!(header_of(copy_of_grandkid).parent_session, Some(new_kid));
        assert!(kid_text.contains(&header_of(copy_of_grandkid).session.to_string()));

        assert_eq!(
            jsonl_in(&agents).len(),
            5,
            "the source keeps its own agents untouched"
        );
        assert!(
            std::fs::read_to_string(agents.join(format!("{kid_id}.jsonl")))
                .unwrap()
                .contains("said after the fork point")
        );
    }

    #[test]
    fn ids_are_renamed_only_where_they_stand_alone() {
        let old = SessionId::new().to_string();
        let renames = HashMap::from([(old.clone(), "NEW".to_owned())]);
        assert_eq!(rename_ids(&format!("a {old} b"), &renames), "a NEW b");
        assert_eq!(rename_ids(&format!("\"{old}\""), &renames), "\"NEW\"");
        assert_eq!(rename_ids(&format!("X{old}"), &renames), format!("X{old}"));
        assert_eq!(rename_ids(&format!("{old}9"), &renames), format!("{old}9"));
        assert_eq!(rename_ids("\u{e9} short", &renames), "\u{e9} short");
        assert_eq!(
            rename_ids(&format!(r"line\n{old}"), &renames),
            r"line\nNEW",
            "after a JSON escape"
        );
        assert_eq!(
            rename_ids(&format!("worktrees/{old}"), &renames),
            format!("worktrees/{old}"),
            "a worktree path stays"
        );
        assert_eq!(
            rename_ids(&format!(r"worktrees\\{old}"), &renames),
            format!(r"worktrees\\{old}"),
            "on Windows too"
        );
        assert_eq!(
            rename_ids(&format!("\"{old}/output.txt\""), &renames),
            format!("\"{old}/output.txt\""),
            "nor where it starts a path"
        );
        assert_eq!(
            rename_ids(&format!(r"{old}\\out"), &renames),
            format!(r"{old}\\out")
        );
        assert_eq!(
            rename_ids(&format!(r#"session=\"{old}\""#), &renames),
            r#"session=\"NEW\""#,
            "an escaped quote still closes it"
        );
    }

    /// A source with one agent its history names, as (source, fork
    /// point, agents directory, agent file).
    fn source_with_an_agent(project: &Path) -> (PathBuf, EntryId, PathBuf, PathBuf) {
        let root = fresh_header("m");
        let root_id = root.session;
        let src = project.join(format!("{root_id}.jsonl"));
        let agents = crate::layout::agents_dir(&src);
        std::fs::create_dir_all(&agents).unwrap();
        let kid_id = header_only_agent(&agents, root_id, 0);
        let reply = message_entry(
            Role::Assistant,
            &format!("<agent session=\"{kid_id}\">done</agent>"),
        );
        let at = reply.id();
        write(&src, root, &[reply]);
        (
            src,
            at,
            agents.clone(),
            agents.join(format!("{kid_id}.jsonl")),
        )
    }

    #[test]
    fn a_corrupt_agent_line_ends_its_copy() {
        let dir = tempdir().unwrap();
        let (src, at, _, kid) = source_with_an_agent(dir.path());
        let mut text = std::fs::read_to_string(&kid).unwrap();
        text.push_str("not json\n");
        std::fs::write(&kid, text).unwrap();
        let fork_id = SessionId::new();
        let dst = dir.path().join(format!("{fork_id}.jsonl"));
        fork(&src, &dst, fork_id, at).unwrap();
        let copied = jsonl_in(&crate::layout::agents_dir(&dst));
        assert_eq!(copied.len(), 1);
        assert!(
            crate::replay(&copied[0]).is_ok(),
            "the copy reads back whole"
        );
    }

    #[test]
    fn a_blank_agent_line_is_skipped_not_the_end() {
        let dir = tempdir().unwrap();
        let (src, _, _, kid) = source_with_an_agent(dir.path());
        let mut text = std::fs::read_to_string(&kid).unwrap();
        text.push('\n');
        std::fs::write(&kid, text).unwrap();
        SessionWriter::open(&kid)
            .unwrap()
            .append(&message_entry(Role::User, "after the blank"))
            .unwrap();
        let later = message_entry(Role::User, "later");
        let at = later.id();
        SessionWriter::open(&src).unwrap().append(&later).unwrap();

        let fork_id = SessionId::new();
        let dst = dir.path().join(format!("{fork_id}.jsonl"));
        fork(&src, &dst, fork_id, at).unwrap();
        let copied = jsonl_in(&crate::layout::agents_dir(&dst));
        assert!(
            std::fs::read_to_string(&copied[0])
                .unwrap()
                .contains("after the blank")
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_agent_fails_the_fork_only_when_the_history_names_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let (src, at, agents, kid) = source_with_an_agent(dir.path());
        let stray = agents.join(format!("{}.jsonl", SessionId::new()));
        std::fs::write(&stray, b"").unwrap();
        std::fs::set_permissions(&stray, std::fs::Permissions::from_mode(0o000)).unwrap();
        let first = SessionId::new();
        fork(&src, &dir.path().join(format!("{first}.jsonl")), first, at).unwrap();
        if std::fs::read(&stray).is_ok() {
            // Running as root, which reads any file.
            return;
        }
        std::fs::set_permissions(&kid, std::fs::Permissions::from_mode(0o000)).unwrap();
        let second = SessionId::new();
        let dst = dir.path().join(format!("{second}.jsonl"));
        let forked = fork(&src, &dst, second, at);
        std::fs::set_permissions(&kid, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(forked.is_err(), "the named agent cannot be copied");
        assert!(!dst.exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_agents_directory_fails_the_fork_before_it_starts() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let (src, at, agents, _) = source_with_an_agent(dir.path());
        std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o000)).unwrap();
        let fork_id = SessionId::new();
        let dst = dir.path().join(format!("{fork_id}.jsonl"));
        let forked = fork(&src, &dst, fork_id, at);
        std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700)).unwrap();
        if std::fs::read_dir(&agents).is_ok() && forked.is_ok() {
            // Running as root, which reads any directory.
            return;
        }
        assert!(forked.is_err());
        assert!(!dst.exists(), "no fork that would lose the agent");
    }

    #[test]
    fn a_fork_of_an_agent_copies_the_agents_it_started() {
        let dir = tempdir().unwrap();
        let project = dir.path().join("kage-personal-0123456789");
        let root_id = SessionId::new();
        let agents = project
            .join(root_id.to_string())
            .join(crate::layout::AGENTS_DIR);
        std::fs::create_dir_all(&agents).unwrap();
        let mut kid = fresh_header("m");
        kid.parent_session = Some(root_id);
        let kid_id = kid.session;
        let mut grandkid = fresh_header("m");
        grandkid.parent_session = Some(kid_id);
        let grandkid_id = grandkid.session;
        write(&agents.join(format!("{grandkid_id}.jsonl")), grandkid, &[]);
        let src = agents.join(format!("{kid_id}.jsonl"));
        let reply = message_entry(
            Role::Assistant,
            &format!("<agent session=\"{grandkid_id}\">done</agent>"),
        );
        let at = reply.id();
        write(&src, kid, &[reply]);

        let fork_id = SessionId::new();
        let dst = project.join(format!("{fork_id}.jsonl"));
        fork(&src, &dst, fork_id, at).unwrap();
        let copied: Vec<PathBuf> = jsonl_in(&crate::layout::agents_dir(&dst));
        assert_eq!(copied.len(), 1, "the agent's own agent, not itself");
        let header = header_of(&copied[0]);
        assert_eq!(header.parent_session, Some(fork_id));
        assert!(
            std::fs::read_to_string(&dst)
                .unwrap()
                .contains(&header.session.to_string())
        );
    }
}
