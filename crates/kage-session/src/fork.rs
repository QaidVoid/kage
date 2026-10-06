//! Branch a new session from an existing one at a specific entry.
//!
//! [`fork`] copies the source's entries verbatim from the start of the file
//! up to and including the entry with id `at`. The destination header
//! carries a fresh [`SessionId`], a freshly-minted creation timestamp, and
//! `parent_session` / `parent_entry` fields linking back to the source.
//! All other header fields (cwd, model, system prompt) are inherited.
//!
//! Forking does not mutate the source. After a fork, both files exist
//! independently and may diverge.

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::entry::{EntryId, FORMAT_VERSION, Header, SessionEntry, SessionId};
use crate::error::SessionError;
use crate::reader::SessionReader;
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
/// if `dst` already exists, or if no entry in `src` has id `at`.
pub fn fork(
    src: &Path,
    dst: &Path,
    new_session: SessionId,
    at: EntryId,
) -> Result<bool, SessionError> {
    let mut reader = SessionReader::iter(src)?;
    let first = reader.next().ok_or_else(|| SessionError::Empty {
        path: src.to_path_buf(),
    })??;
    let SessionEntry::Header(parent_header) = first else {
        return Err(SessionError::MissingHeader {
            path: src.to_path_buf(),
        });
    };

    let new_header = Header {
        version: FORMAT_VERSION,
        session: new_session,
        id: EntryId::new(),
        ts: Utc::now(),
        cwd: parent_header.cwd.clone(),
        model: parent_header.model.clone(),
        system_prompt: parent_header.system_prompt.clone(),
        parent_session: Some(parent_header.session),
        parent_entry: Some(at),
    };
    fork_as(src, dst, new_header, at)
}

/// Fork `src` into a new session file at `dst` under the exact header
/// the caller supplies, copying entries up to and including the entry
/// whose id equals `at`. The header's `session` should be the id the
/// caller used for `dst` (so the file name and the in-file id agree).
/// Use this instead of [`fork`] when the new session needs its own
/// model or system prompt, as forked agent children do.
///
/// Returns whether the source's tail was torn: the copy then ends at
/// the last whole entry before it.
///
/// # Errors
///
/// Errors if `src` cannot be read, if its first entry is not a header,
/// if `dst` already exists, or if no entry in `src` has id `at`.
pub fn fork_as(
    src: &Path,
    dst: &Path,
    new_header: Header,
    at: EntryId,
) -> Result<bool, SessionError> {
    let snapshot = snapshot(src, |entry| entry.id() == at)?;
    if snapshot.at != at {
        return Err(SessionError::EntryNotFound {
            path: src.to_path_buf(),
            at,
        });
    }
    let truncated = snapshot.truncated;
    snapshot.write(dst, new_header)?;
    Ok(truncated)
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
}

impl Snapshot {
    /// Whether any entry past the header was kept.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// Create the session file `dst` under `header` holding these
    /// entries, with one fsync for all of them, and keep it open for
    /// appending.
    ///
    /// # Errors
    ///
    /// Errors if `dst` already exists or cannot be written.
    pub fn write(&self, dst: &Path, header: Header) -> Result<SessionWriter, SessionError> {
        let mut writer = SessionWriter::create(PathBuf::from(dst), header)?;
        if !self.is_empty() {
            writer.append_lines(&self.lines)?;
        }
        Ok(writer)
    }
}

/// Read `src` once and keep its entries up to the last one `keep`
/// accepts. A trailing line that does not parse is a torn write and
/// ends the file, as it does for [`SessionReader`]; the snapshot's
/// `truncated` flag reports it.
///
/// # Errors
///
/// Errors if `src` cannot be read, if its first entry is not a header,
/// or if a line before the last does not parse.
pub fn snapshot(
    src: &Path,
    keep: impl Fn(&SessionEntry) -> bool,
) -> Result<Snapshot, SessionError> {
    let data = std::fs::read(src).map_err(|err| SessionError::Io {
        path: src.to_path_buf(),
        source: err,
    })?;
    let mut lines = data.split_inclusive(|byte| *byte == b'\n').peekable();
    let mut offset = 0;
    let header_id = loop {
        let Some(line) = lines.next() else {
            return Err(SessionError::Empty {
                path: src.to_path_buf(),
            });
        };
        offset += line.len();
        if line.trim_ascii().is_empty() {
            continue;
        }
        match serde_json::from_slice::<SessionEntry>(line) {
            Ok(SessionEntry::Header(header)) => break header.id,
            _ => {
                return Err(SessionError::MissingHeader {
                    path: src.to_path_buf(),
                });
            }
        }
    };
    let start = offset;
    let mut kept = (header_id, start);
    let mut truncated = false;
    let mut line_no = 1;
    while let Some(line) = lines.next() {
        line_no += 1;
        offset += line.len();
        if line.trim_ascii().is_empty() {
            continue;
        }
        match serde_json::from_slice::<SessionEntry>(line) {
            Ok(entry) if keep(&entry) => kept = (entry.id(), offset),
            Ok(_) => {}
            Err(_) if lines.peek().is_none() => {
                truncated = true;
                break;
            }
            Err(err) => {
                return Err(SessionError::Decode {
                    path: src.to_path_buf(),
                    line: line_no,
                    source: err,
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
    })
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
        EntryId, FORMAT_VERSION, Header, Label, MessageEntry, SessionEntry, SessionId,
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
    fn a_whole_snapshot_reports_no_truncation() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.jsonl");
        let m1 = message_entry(Role::User, "one");
        write(&src, fresh_header("x:y"), &[m1.clone()]);
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
        assert_eq!(
            fork(&torn, &dir.path().join("torn-fork.jsonl"), torn_id, torn_at).unwrap(),
            true,
            "the torn tail is reported"
        );
        let whole_id = SessionId::new();
        assert_eq!(
            fork(
                &whole,
                &dir.path().join("whole-fork.jsonl"),
                whole_id,
                torn_at
            )
            .unwrap(),
            false,
            "an intact source is not truncated"
        );
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
            anchor: anchor.clone(),
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
}
